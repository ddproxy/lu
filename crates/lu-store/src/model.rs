//! Randomised model-based tests for any [`Store`]. Each seed replays a fixed operation script
//! against a fresh store and checks the lifecycle invariants after every step.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use lu_proto::{
    ArtifactName, Completion, FailureClass, HarnessState, Heartbeat, JobId, JobSpec, JobState,
    NodeName, Outcome, ProviderState, MAX_LOG_BYTES,
};

use crate::conformance::hb;
use crate::{JobRecord, Now, Store, StoreError};

const OPS: usize = 300;
const LONG_OPS: usize = 3_000;
const MAX_JOBS: usize = 24;
const LEASE_MS: u64 = 1_000;
const GRACE_MS: u64 = 1_500;
const MAX_RETRIES: u32 = 1;
const CORDON_MS: u64 = 1_200;

const NODE_NAMES: [&str; 3] = ["node-a", "node-b", "node-c"];
const PROVIDERS: [&str; 2] = ["prov-a", "prov-b"];

/// Fixed seeds for the normal run, generated once so a failure always reproduces.
pub const SEEDS: [u64; 200] = [
    0x0BAD5EED6A8B2C51,
    0x1F3C9A7D2E4B6085,
    0x2D7E41B8C09536AF,
    0x3A6B0F52D8C147E3,
    0x4C81D3A96F205B7D,
    0x5E2F78C4B1D6903A,
    0x6B94E05A7C381F2D,
    0x79C3A6F18E42D5B0,
    0x8D5B2E60A3F719C4,
    0x9A07C4D3E68B215F,
    0xA3E96B14D50F82C7,
    0xB7C28F5D0A3E1694,
    0xC5D1A73E8F206B49,
    0xD8F04C29B6E5137A,
    0xE2B85D9F4C07A136,
    0xF6A13C70D9E4825B,
    0x14E7B2A8D053C96F,
    0x27B5D049E8C316A2,
    0x3C90F7B6A4D285E1,
    0x4A2D86C1F07B593E,
    0x58F3E1B70C9A246D,
    0x69A4C852D1F03B7E,
    0x7B06D9E3A5C4812F,
    0x8C3B1E60F7D592A4,
    0x96F82B4D3A0C71E5,
    0xA1C5E93F6B280D74,
    0xB40D27A8F1E36C95,
    0xC86F4B1D9A05E273,
    0xD59A03C7E2B81F46,
    0xE7C2485B0F6D93A1,
    0xF2E81D6A4C079B35,
    0x03A6F9C24E1B58D7,
    0x1B48D7E05C93A26F,
    0x2E93A05F8D1C6B47,
    0x37D1C64B0A8F92E5,
    0x4D28E1B0F63C5A79,
    0x5A6F04D7C2E1938B,
    0x6C07B3E58A1D4F96,
    0x7E91D4A0F62B37C5,
    0x8F2C58B3E04D197A,
    0x90B7E4D16F3A82C5,
    0xA48F12C7B5E6039D,
    0xB6E0C93A4D8175F2,
    0xC1A75F28E4B306D9,
    0xD7F31B6A0C9582E4,
    0xE8A402D5C71F63B9,
    0xF5B9C82E1A406D73,
    0x09C47E3B2F8A15D6,
    0x16D0A5F49E73C28B,
    0x2A5C81E7B30D94F6,
    0x3F82D6A0C49E157B,
    0x4B1E97C5A2F068D3,
    0x5D70A3E9B45C821F,
    0x6E29D1B8F04A7C53,
    0x78C4F0A6D31B95E2,
    0x8A91E5C72D0F368B,
    0x9C3B78A1F05E2D46,
    0xA6D02C9B4E87F135,
    0xB2F8A5D19C036E74,
    0xC9E14B7A0D68F325,
    0xDA5C20F8B4E79163,
    0xE3B76D1C5A0F2849,
    0xF1E0A48D3C92B657,
    0x045C9E2A7B1D6038,
    0x1D8B3F60C52A97E4,
    0x2F16C4A8E09D352B,
    0x3E80B5D2F1A64C97,
    0x4A97E31C6B0D58F2,
    0x5C24F8A0D1B639E7,
    0x6F83A1E5C07D492B,
    0x71D6B904A3F28E5C,
    0x8E40C27B1A95D6F3,
    0x9B0F58D4E2C317A6,
    0xA2D6E13B8F0A74C5,
    0xBE71C4A0F52D8963,
    0xC0A93E7B4D1862F5,
    0xD46F2B80A9E1C537,
    0xE9C105D7B3A4F682,
    0xF8B2A6C41E0D5937,
    0x06E1C3B7A48F250D,
    0x14A7D2F0B9C6853E,
    0x25F0B8A4C61D397E,
    0x3B9D5E02A7F4C186,
    0x48C3A0F7D5219B6E,
    0x57E9B4D2A0C6381F,
    0x6A0D7C3E9F2B5481,
    0x7C58E1A4B0D936F2,
    0x8B2F6D07C4A159E3,
    0x9D64A3C1F80B275E,
    0xAC81E5B3D09F4726,
    0xB5D0F2A8C1E46B97,
    0xC72E9B5A0D48F163,
    0xD90C4E1B7A2F5863,
    0xE4A8D306B1C795F2,
    0xF3C60E9A4B71D285,
    0x0A2B5D8F1E37C964,
    0x18F4C7A2D30B69E5,
    0x269B1E50A7D4F283,
    0x354D8C0E2A6F91B7,
    0x4F0A7D3B9C15E286,
    0x5B92C6F08D3A5417,
    0x6D41A8E2B70C95F3,
    0x7A8E30D5C1F4269B,
    0x8C6F1B4A9E2073D5,
    0x9E05C2D7B48A316F,
    0xA7D9E6C0F5B14283,
    0xB1A4F8E3C06D5972,
    0xC2E71D5B90A8F346,
    0xD6B048C2F1E57A93,
    0xE1C93A6D5B0F8274,
    0xF4E70B1A8C26D395,
    0x02F5A8C61D947E3B,
    0x1C60E3B5A2D7198F,
    0x2B87D1F40C596A2E,
    0x39E2A5B8D10F7463,
    0x46C1F30A8B5D297E,
    0x5F8B2E6C1A07D453,
    0x68A4D0B7E3F215C9,
    0x7D30C9A5B62E1F48,
    0x8D1F6A2C0B9537E4,
    0x93C58B0E7A4D162F,
    0xA9E2F31D5C806B47,
    0xB8C74E0A2F1695D3,
    0xC3F19B5D7E0A2468,
    0xD281C0A4E6F5379B,
    0xEA5D3C1B8F027496,
    0xF0B6E2A9D14C3875,
    0x054E7A1C3B9D60F2,
    0x1E9A2F5C8D04B763,
    0x28C50B1E4A6F93D7,
    0x3A1F7D2B0E5C8649,
    0x4B86C3E0D2A5197F,
    0x59D0E4B2A1C3786F,
    0x6E3A1F5C9B0D7284,
    0x7F02B6A4D8C1395E,
    0x8A4D7C1B6E92F053,
    0x9F51A3D0C7B6248E,
    0xA1E8C4B30D5F9276,
    0xBC30D5A7E1F0864B,
    0xC4D91F2B6A0E7385,
    0xD3A62C8E5F01B479,
    0xEC04B7D1A5C29836,
    0xF9D2E60C4A1B7538,
    0x07C3A5E9B0D1462F,
    0x15B8E0D4C2A3796F,
    0x2C71F4A0B9D53E86,
    0x3D08B2C6F5A1E947,
    0x4E96A1D3C07B258F,
    0x5A3F8D0B1E4C7692,
    0x67C2E9A5D3F14B80,
    0x749A0C7D2E5B1863,
    0x82E5D1B4A0F6937C,
    0x9B76C0A3E5D1284F,
    0xA0C8E2F5B7D46913,
    0xB3D5A70E1C4F8296,
    0xCA1F6B0D3E8A5742,
    0xD7B4E3A9C1F05268,
    0xE6C02F7B5A1D9384,
    0xF3A9D4C1E608B275,
    0x0D57C1B8E4A3096F,
    0x1A82D5F0C3B7469E,
    0x2F4B9E1A7C05D386,
    0x38F0A6D3B2E5741C,
    0x4D19C4B7E2F0A365,
    0x5B60F2A8D9E1437C,
    0x6A3D8B0E5C1F7294,
    0x7B95C6E0A1D4382F,
    0x8C10F3B7D5A2964E,
    0x9D87A4E2C6B0135F,
    0xA5B2C8D0F7E14936,
    0xB9F0E1A4C3D6872B,
    0xC6A3D7B0E9F25418,
    0xDB1E5C2A0F7B3684,
    0xE8B4F1C6D0A39527,
    0xF4D06A8B3E1C7592,
    0x0E31A7D5B2F0846C,
    0x1D6F2C9E4B8A0573,
    0x26B9E0A7D3C4851F,
    0x3C4A1F8D6E0B5297,
    0x4F2E8B3C7A1D6049,
    0x5D05C4E9A2F3768B,
    0x6B78A1D0F5E293C4,
    0x7E12D6B8C0A34759,
    0x87A0C5E3D1F4628B,
    0x9C29F4A7B6D0135E,
    0xA8D3C0E1F5B27649,
    0xB4F6A2D9C8E03175,
    0xC1E847B0A3D5962F,
    0xD0A5C3E7F2B4186D,
    0xE2F6B1A0D9C34578,
    0xF1C84D5E0A2B6937,
    0x0B9D2A5F8C4E1376,
    0x17E0C3A9D6F2485B,
    0x2A3C8B5E1D07F496,
    0x3987D0B4A2E5617C,
    0x42F1C6E0B8D5394A,
    0x5C8A2B4F0E3D9756,
    0x6D0E5F1A8C7B2349,
    0x7A47C3E9B5D1062F,
    0x8E09A4D2C6F1375B,
];

/// A fixed subset used by the ignored long run.
pub const LONG_SEEDS: [u64; 24] = [
    0x5E2F78C4B1D6903A,
    0x8D5B2E60A3F719C4,
    0xB7C28F5D0A3E1694,
    0x14E7B2A8D053C96F,
    0x4A2D86C1F07B593E,
    0x96F82B4D3A0C71E5,
    0xC86F4B1D9A05E273,
    0x1B48D7E05C93A26F,
    0x5A6F04D7C2E1938B,
    0x90B7E4D16F3A82C5,
    0xD7F31B6A0C9582E4,
    0x16D0A5F49E73C28B,
    0x4B1E97C5A2F068D3,
    0x8A91E5C72D0F368B,
    0xDA5C20F8B4E79163,
    0x045C9E2A7B1D6038,
    0x4A97E31C6B0D58F2,
    0x8E40C27B1A95D6F3,
    0xC0A93E7B4D1862F5,
    0x06E1C3B7A48F250D,
    0x3B9D5E02A7F4C186,
    0x7C58E1A4B0D936F2,
    0xAC81E5B3D09F4726,
    0xD90C4E1B7A2F5863,
];

type SharedLog = Arc<Mutex<Vec<String>>>;

fn node(name: &str) -> NodeName {
    NodeName::from_str(name).unwrap()
}

fn active(state: JobState) -> bool {
    matches!(
        state,
        JobState::Leased | JobState::Running | JobState::Recovering
    )
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

/// Runs the normal model test against a store created by `make`.
pub fn run<S: Store>(make: impl Fn() -> S) {
    for seed in SEEDS {
        run_one(&make, seed, OPS);
    }
}

/// Runs the longer variant; ignored in the normal test run.
pub fn run_long<S: Store>(make: impl Fn() -> S) {
    for seed in LONG_SEEDS {
        run_one(&make, seed, LONG_OPS);
    }
}

fn run_one<S: Store>(make: &impl Fn() -> S, seed: u64, ops: usize) {
    let log: SharedLog = Arc::new(Mutex::new(Vec::new()));
    let model_log = Arc::clone(&log);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let store = make();
        let mut model = Model::new(seed, ops, store, model_log);
        model.run();
    }));
    if let Err(payload) = result {
        let message = panic_message(&payload);
        let lines = log.lock().map(|l| l.join("\n")).unwrap_or_default();
        panic!("model failed for seed {seed} (0x{seed:016X}):\n{lines}\n\n{message}");
    }
}

fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

struct Model<S: Store> {
    store: S,
    seed: u64,
    ops: usize,
    rng: Rng,
    now: Now,
    log: SharedLog,
    seq: u64,
    records: BTreeMap<JobId, JobRecord>,
    nodes: BTreeMap<NodeName, Heartbeat>,
    node_generation: BTreeMap<NodeName, u64>,
    draining: BTreeSet<NodeName>,
    lease_until: BTreeMap<JobId, u64>,
    recovering_since: BTreeMap<JobId, u64>,
    cordons: BTreeMap<(NodeName, String), u64>,
    assigned_hb: BTreeMap<JobId, u64>,
    node_retries: BTreeMap<JobId, u32>,
    artifacts: BTreeMap<JobId, BTreeSet<&'static str>>,
    artifact_epochs: BTreeMap<(JobId, &'static str), u64>,
    key_specs: BTreeMap<String, (JobSpec, JobId)>,
    dependents: BTreeMap<JobId, BTreeSet<JobId>>,
    dirty: BTreeSet<JobId>,
    pending_terminal: Option<BTreeMap<JobId, JobRecord>>,
}

impl<S: Store> Model<S> {
    fn new(seed: u64, ops: usize, store: S, log: SharedLog) -> Self {
        let mut model = Self {
            store,
            seed,
            ops,
            rng: Rng::new(seed),
            now: Now {
                wall_unix: 1_700_000_000,
                mono_ms: 1_000,
            },
            log,
            seq: 0,
            records: BTreeMap::new(),
            nodes: BTreeMap::new(),
            node_generation: BTreeMap::new(),
            draining: BTreeSet::new(),
            lease_until: BTreeMap::new(),
            recovering_since: BTreeMap::new(),
            cordons: BTreeMap::new(),
            assigned_hb: BTreeMap::new(),
            node_retries: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            artifact_epochs: BTreeMap::new(),
            key_specs: BTreeMap::new(),
            dependents: BTreeMap::new(),
            dirty: BTreeSet::new(),
            pending_terminal: None,
        };
        for name in NODE_NAMES {
            let heartbeat = hb(
                name,
                HarnessState::Idle,
                vec![
                    (PROVIDERS[0], ProviderState::Ok),
                    (PROVIDERS[1], ProviderState::Ok),
                ],
            );
            model
                .store
                .heartbeat(&heartbeat, model.now)
                .expect("bootstrap heartbeat");
            let name = node(name);
            model.nodes.insert(name.clone(), heartbeat);
            model.node_generation.insert(name.clone(), 0);
            model.note(format!("bootstrap heartbeat {}", name.as_str()));
        }
        model
    }

    fn note(&self, text: impl Into<String>) {
        if let Ok(mut lines) = self.log.lock() {
            lines.push(format!("[{}] {}", self.now.mono_ms, text.into()));
        }
    }

    fn run(&mut self) {
        for step in 0..self.ops {
            self.advance_clock();
            let roll = if step == 0 { 0 } else { self.rng.below(100) };
            match roll {
                0..=13 => self.op_create(),
                14..=21 => self.op_heartbeat(),
                22..=34 => self.op_next_assignment(),
                35..=43 => self.op_start(),
                44..=52 => self.op_renew(),
                53..=58 => self.op_reserve_log(),
                59..=64 => self.op_record_artifact(),
                65..=78 => self.op_complete(),
                79..=86 => self.op_cancel(),
                87..=90 => self.op_drain(),
                91..=93 => self.op_expire(),
                94..=96 => self.op_requeue(),
                _ => self.op_recover_on_boot(),
            }
            self.refresh_and_check();
        }
    }

    fn advance_clock(&mut self) {
        let delta = if self.rng.chance(1, 10) {
            self.rng.below(10_000)
        } else {
            self.rng.below(250)
        };
        self.now.mono_ms = self.now.mono_ms.saturating_add(delta);
        self.now.wall_unix = self
            .now
            .wall_unix
            .saturating_add((delta / 1_000) as i64)
            .saturating_add(self.rng.below(2) as i64);
    }

    fn fresh_spec(&mut self, seq: u64) -> JobSpec {
        let provider = if self.rng.chance(2, 5) {
            Some(PROVIDERS[self.rng.index(PROVIDERS.len())].to_owned())
        } else {
            None
        };
        JobSpec {
            repo: format!("https://example.test/r{seq}.git"),
            git_ref: "main".to_owned(),
            task: format!("model task {seq}"),
            harness: "opencode".to_owned(),
            provider,
            model: None,
            timeout_secs: 3600,
            idle_timeout_secs: None,
            idempotency_key: None,
            after: Vec::new(),
            base_commit: None,
            stack_dependencies: true,
            requires: Vec::new(),
        }
    }

    fn random_deps(&mut self) -> Vec<JobId> {
        if self.records.is_empty() || !self.rng.chance(1, 2) {
            return Vec::new();
        }
        let ids: Vec<JobId> = self.records.keys().cloned().collect();
        let start = self.rng.index(ids.len());
        let mut out = vec![ids[start].clone()];
        if ids.len() > 1 && self.rng.chance(1, 3) {
            let offset = 1 + self.rng.below((ids.len() - 1) as u64) as usize;
            out.push(ids[(start + offset) % ids.len()].clone());
        }
        out
    }

    fn op_create(&mut self) {
        if self.records.len() >= MAX_JOBS {
            self.note("create: skipped, job cap reached");
            return;
        }
        let seq = self.seq;
        self.seq = self.seq.saturating_add(1);

        if self.rng.chance(1, 3) && !self.key_specs.is_empty() {
            let keys: Vec<String> = self.key_specs.keys().cloned().collect();
            let key = keys[self.rng.index(keys.len())].clone();
            let (spec, job_id) = self.key_specs.get(&key).expect("known key").clone();
            if self.rng.chance(1, 2) {
                self.note(format!("create idempotent key={key}"));
                let (got, created) = self
                    .store
                    .create_job(&spec, self.now)
                    .expect("idempotent create");
                assert!(!created, "same key and spec must not create a job");
                assert_eq!(got.id, job_id);
                return;
            }
            let mut spec = self.fresh_spec(seq);
            spec.idempotency_key = Some(key.clone());
            self.note(format!("create conflicting key={key}"));
            let err = self.store.create_job(&spec, self.now).unwrap_err();
            assert_eq!(err, StoreError::Conflict { length: None });
            return;
        }

        let mut spec = self.fresh_spec(seq);
        if self.rng.chance(2, 5) {
            spec.idempotency_key = Some(format!("key-{:016X}-{seq}", self.seed));
        }
        spec.after = self.random_deps();
        let key = spec.idempotency_key.clone();
        let expected_dead = spec.after.iter().find_map(|dep| {
            self.records
                .get(dep)
                .filter(|record| matches!(record.state, JobState::Failed | JobState::Cancelled))
                .map(|_| dep.clone())
        });
        self.note(format!(
            "create repo={} provider={:?} key={:?} after={:?}",
            spec.repo, spec.provider, spec.idempotency_key, spec.after
        ));
        match self.store.create_job(&spec, self.now) {
            Ok((record, created)) => {
                assert!(
                    expected_dead.is_none(),
                    "create accepted a dead dependency {:?}",
                    expected_dead
                );
                assert!(created, "a fresh key must create a job");
                let blocked = spec
                    .after
                    .iter()
                    .any(|dep| self.records[dep].state != JobState::Done);
                let waiting: BTreeSet<JobId> = spec
                    .after
                    .iter()
                    .filter(|dep| self.records[*dep].state != JobState::Done)
                    .cloned()
                    .collect();
                assert_eq!(
                    record.state,
                    if blocked {
                        JobState::Blocked
                    } else {
                        JobState::Queued
                    }
                );
                assert_eq!(
                    record.waiting_on.iter().cloned().collect::<BTreeSet<_>>(),
                    waiting
                );
                assert_eq!(record.attempt, 1);
                assert_eq!(record.epoch, 0);
                assert_eq!(record.node, None);
                assert_eq!(record.finished_at, None);
                assert_eq!(record.log_len, 0);
                assert!(record.artifacts.is_empty());
                assert_eq!(record.spec, spec);
                self.artifacts.insert(record.id.clone(), BTreeSet::new());
                self.node_retries.insert(record.id.clone(), 0);
                for dep in &record.spec.after {
                    self.dependents
                        .entry(dep.clone())
                        .or_default()
                        .insert(record.id.clone());
                }
                self.dirty.insert(record.id.clone());
                if let Some(key) = key {
                    self.key_specs.insert(key, (spec, record.id.clone()));
                }
            }
            Err(err) => {
                let dead = expected_dead.expect("a dead dependency was expected");
                assert_eq!(err, StoreError::DeadDependency(dead));
            }
        }
    }

    fn op_heartbeat(&mut self) {
        let name = NODE_NAMES[self.rng.index(NODE_NAMES.len())];
        let harness = if self.rng.chance(4, 5) {
            HarnessState::Idle
        } else {
            HarnessState::Busy
        };
        let mut providers = Vec::new();
        for provider in PROVIDERS {
            let state = match self.rng.below(100) {
                0..=59 => ProviderState::Ok,
                60..=74 => ProviderState::Cooling,
                75..=84 => ProviderState::Disabled,
                _ => ProviderState::Unknown,
            };
            if self.rng.chance(9, 10) {
                providers.push((provider, state));
            }
        }
        let heartbeat = hb(name, harness, providers.clone());
        self.note(format!(
            "heartbeat {name} harness={harness:?} providers={providers:?}"
        ));
        self.store
            .heartbeat(&heartbeat, self.now)
            .expect("heartbeat");
        let name = node(name);
        self.nodes.insert(name.clone(), heartbeat);
        let generation = self.node_generation.entry(name).or_insert(0);
        *generation = generation.saturating_add(1);
    }

    fn provider_ok(&self, name: &NodeName, provider: &str) -> bool {
        self.nodes.get(name).is_some_and(|heartbeat| {
            heartbeat
                .providers
                .iter()
                .any(|p| p.name == provider && p.state == ProviderState::Ok)
        })
    }

    fn cordon_deadline(&self, name: &NodeName, provider: &str) -> u64 {
        self.cordons
            .get(&(name.clone(), provider.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    fn provider_eligible(&self, name: &NodeName, record: &JobRecord) -> bool {
        match &record.spec.provider {
            None => true,
            Some(provider) => {
                self.provider_ok(name, provider)
                    && self.cordon_deadline(name, provider) <= self.now.mono_ms
            }
        }
    }

    fn active_on(&self, name: &NodeName) -> Option<&JobRecord> {
        self.records
            .values()
            .find(|record| active(record.state) && record.node.as_ref() == Some(name))
    }

    fn op_next_assignment(&mut self) {
        let name = NODE_NAMES[self.rng.index(NODE_NAMES.len())];
        let node_name = node(name);
        let held = self.active_on(&node_name).map(|r| r.id.clone());
        let harness_busy = self
            .nodes
            .get(&node_name)
            .is_some_and(|h| h.harness != HarnessState::Idle);
        let predicted = if self.draining.contains(&node_name) || harness_busy || held.is_some() {
            None
        } else {
            self.records
                .values()
                .find(|record| {
                    record.state == JobState::Queued && self.provider_eligible(&node_name, record)
                })
                .map(|record| (record.id.clone(), record.epoch))
        };
        self.note(format!(
            "next_assignment {name} draining={} busy={} held={held:?} predicted={predicted:?}",
            self.draining.contains(&node_name),
            harness_busy
        ));
        let got = self
            .store
            .next_assignment(&node_name, self.now, LEASE_MS)
            .expect("next_assignment");
        match (predicted, got) {
            (None, None) => {}
            (Some((id, epoch)), Some(assignment)) => {
                self.dirty.insert(id.clone());
                assert_eq!(assignment.job_id, id, "unexpected job assigned");
                assert_eq!(assignment.epoch, epoch + 1, "assignment must bump epoch");
                let old = self.records.get(&id).expect("assigned job is known");
                assert_eq!(old.state, JobState::Queued);
                assert_eq!(assignment.spec, old.spec);
                assert!(
                    !self.draining.contains(&node_name),
                    "assigned work to a draining node"
                );
                assert!(held.is_none(), "node already held an active job");
                if let Some(provider) = &assignment.spec.provider {
                    assert!(
                        self.provider_ok(&node_name, provider),
                        "assigned provider {provider} that the node does not report ok"
                    );
                    assert!(
                        self.cordon_deadline(&node_name, provider) <= self.now.mono_ms,
                        "assigned provider {provider} under an unexpired cordon"
                    );
                }
                self.lease_until
                    .insert(id.clone(), self.now.mono_ms.saturating_add(LEASE_MS));
                self.recovering_since.remove(&id);
                let generation = self.node_generation.get(&node_name).copied().unwrap_or(0);
                self.assigned_hb.insert(id, generation);
            }
            (Some((id, _)), None) => {
                panic!("store returned no assignment; model expected {id}");
            }
            (None, Some(assignment)) => {
                panic!(
                    "store assigned {} to {name}; model expected none (held {held:?})",
                    assignment.job_id
                );
            }
        }
    }

    fn op_start(&mut self) {
        let candidates: Vec<JobId> = self
            .records
            .values()
            .filter(|record| matches!(record.state, JobState::Leased | JobState::Recovering))
            .map(|record| record.id.clone())
            .collect();
        if candidates.is_empty() {
            self.note("start: no leased or recovering job");
            return;
        }
        let id = candidates[self.rng.index(candidates.len())].clone();
        let record = self.records.get(&id).expect("target").clone();
        let wrong_epoch = self.rng.chance(1, 8);
        let epoch = if wrong_epoch {
            if record.epoch == 0 {
                1
            } else {
                record.epoch - 1
            }
        } else {
            record.epoch
        };
        self.note(format!(
            "start {id} state={:?} epoch={epoch} actual={}",
            record.state, record.epoch
        ));
        self.dirty.insert(id.clone());
        let result = self.store.start_job(&id, epoch, self.now);
        if wrong_epoch {
            assert_eq!(result.unwrap_err(), StoreError::StaleEpoch);
            return;
        }
        let got = result.expect("start with the current epoch");
        assert_eq!(got.state, JobState::Running);
        assert_eq!(got.epoch, record.epoch);
        assert_eq!(got.node, record.node);
        assert_eq!(got.finished_at, None);
        assert!(got.started_at.is_some());
        self.recovering_since.remove(&id);
    }

    fn op_renew(&mut self) {
        let candidates: Vec<JobId> = self
            .records
            .values()
            .filter(|record| active(record.state))
            .map(|record| record.id.clone())
            .collect();
        if candidates.is_empty() {
            self.note("renew: no active job");
            return;
        }
        let id = candidates[self.rng.index(candidates.len())].clone();
        let record = self.records.get(&id).expect("target").clone();
        let wrong_epoch = self.rng.chance(1, 8);
        let epoch = if wrong_epoch {
            if record.epoch == 0 {
                1
            } else {
                record.epoch - 1
            }
        } else {
            record.epoch
        };
        self.note(format!(
            "renew {id} state={:?} epoch={epoch} cancel={}",
            record.state, record.cancel_requested
        ));
        self.dirty.insert(id.clone());
        let result = self.store.renew_lease(&id, epoch, self.now, LEASE_MS);
        if wrong_epoch {
            assert_eq!(result.unwrap_err(), StoreError::StaleEpoch);
            return;
        }
        let cancel = result.expect("renew with the current epoch");
        assert_eq!(cancel, record.cancel_requested);
        self.lease_until
            .insert(id.clone(), self.now.mono_ms.saturating_add(LEASE_MS));
        self.recovering_since.remove(&id);
    }

    fn op_reserve_log(&mut self) {
        if self.records.is_empty() {
            self.note("reserve_log: no job");
            return;
        }
        let ids: Vec<JobId> = self.records.keys().cloned().collect();
        let id = ids[self.rng.index(ids.len())].clone();
        let record = self.records.get(&id).expect("target").clone();
        let wrong_epoch = self.rng.chance(1, 8);
        let epoch = if wrong_epoch {
            if record.epoch == 0 {
                1
            } else {
                record.epoch - 1
            }
        } else {
            record.epoch
        };
        let wrong_offset = !wrong_epoch && active(record.state) && self.rng.chance(1, 5);
        let offset = if wrong_offset {
            if record.log_len == 0 {
                1
            } else {
                record.log_len - 1
            }
        } else {
            record.log_len
        };
        let n = if self.rng.chance(1, 10) {
            MAX_LOG_BYTES.saturating_add(17)
        } else {
            self.rng.below(257)
        };
        self.note(format!(
            "reserve_log {id} state={:?} epoch={epoch} offset={offset} n={n}",
            record.state
        ));
        self.dirty.insert(id.clone());
        let result = self.store.reserve_log(&id, epoch, offset, n);
        if wrong_epoch {
            assert_eq!(result.unwrap_err(), StoreError::StaleEpoch);
        } else if !active(record.state) {
            assert_eq!(
                result.unwrap_err(),
                StoreError::Conflict { length: None },
                "a terminal or pending job cannot take log bytes"
            );
        } else if wrong_offset {
            assert_eq!(
                result.unwrap_err(),
                StoreError::Conflict {
                    length: Some(record.log_len)
                }
            );
        } else {
            let got = result.expect("reserve_log");
            let expected = record.log_len.saturating_add(n).min(MAX_LOG_BYTES);
            assert_eq!(got, expected, "reserve_log length");
        }
    }

    fn op_record_artifact(&mut self) {
        let candidates: Vec<JobId> = self
            .records
            .values()
            .filter(|record| active(record.state))
            .map(|record| record.id.clone())
            .collect();
        if candidates.is_empty() {
            self.note("record_artifact: no active job");
            return;
        }
        let id = candidates[self.rng.index(candidates.len())].clone();
        let record = self.records.get(&id).expect("target").clone();
        let name = ArtifactName::ALL[self.rng.index(ArtifactName::ALL.len())];
        let stale = record.epoch > 0 && self.rng.chance(1, 5);
        let epoch = if stale {
            record.epoch - 1
        } else {
            record.epoch
        };
        self.note(format!(
            "record_artifact {id} epoch={epoch} actual={} name={}",
            record.epoch,
            name.file_name()
        ));
        self.dirty.insert(id.clone());
        let result = self.store.record_artifact(&id, epoch, name);
        if stale {
            assert_eq!(result.unwrap_err(), StoreError::StaleEpoch);
            return;
        }
        result.expect("record_artifact with the current epoch");
        self.artifacts
            .entry(id.clone())
            .or_default()
            .insert(name.file_name());
        self.artifact_epochs
            .insert((id, name.file_name()), record.epoch);
    }

    fn op_complete(&mut self) {
        let candidates: Vec<JobId> = self
            .records
            .values()
            .filter(|record| active(record.state))
            .map(|record| record.id.clone())
            .collect();
        if candidates.is_empty() {
            self.note("complete: no active job");
            return;
        }
        let mut id = candidates[self.rng.index(candidates.len())].clone();
        let mut record = self.records.get(&id).expect("target").clone();
        let wrong_epoch = self.rng.chance(1, 10);
        if wrong_epoch {
            let epoch = if record.epoch == 0 {
                1
            } else {
                record.epoch - 1
            };
            self.note(format!(
                "complete stale {id} epoch={epoch} actual={}",
                record.epoch
            ));
            let completion = Completion {
                job_id: id.clone(),
                epoch,
                outcome: Outcome::Done,
                exit_code: Some(0),
                summary: None,
                retry_after_secs: None,
            };
            assert_eq!(
                self.store
                    .complete(&completion, None, self.now, MAX_RETRIES, CORDON_MS)
                    .unwrap_err(),
                StoreError::StaleEpoch
            );
            return;
        }

        let roll = self.rng.below(100);
        let mut outcome = match roll {
            0..=29 => Outcome::Done,
            30..=49 => Outcome::Failed(FailureClass::Execution),
            50..=69 => Outcome::Failed(FailureClass::Node),
            70..=89 => Outcome::Failed(FailureClass::Provider),
            90..=94 => Outcome::Failed(FailureClass::Dependency),
            _ => Outcome::Cancelled,
        };
        // `leased -> done` is not an edge: an agent reports done only after starting, so use a
        // job that has started (or downgrade to a failure the lease may legally report).
        if outcome == Outcome::Done && record.state == JobState::Leased {
            let started = self.records.iter().find_map(|(other_id, other)| {
                matches!(other.state, JobState::Running | JobState::Recovering)
                    .then(|| (other_id.clone(), other.clone()))
            });
            match started {
                Some((other_id, other)) => {
                    self.note(format!(
                        "complete {id} is leased; done reported by {other_id}"
                    ));
                    id = other_id;
                    record = other;
                }
                None => outcome = Outcome::Failed(FailureClass::Execution),
            }
        }
        let failure = match outcome {
            Outcome::Failed(class) => Some((class, record.spec.provider.clone())),
            _ => None,
        };
        let retry_after_secs = match outcome {
            Outcome::Failed(FailureClass::Provider) if self.rng.chance(1, 3) => {
                Some(self.rng.below(5_000))
            }
            _ => None,
        };
        let completion = Completion {
            job_id: id.clone(),
            epoch: record.epoch,
            outcome,
            exit_code: match outcome {
                Outcome::Done => Some(0),
                Outcome::Failed(_) => Some(1),
                Outcome::Cancelled => None,
            },
            summary: Some(format!("model completion {roll}")),
            retry_after_secs,
        };
        self.note(format!(
            "complete {id} state={:?} attempt={} outcome={outcome:?} retry_after={retry_after_secs:?}",
            record.state, record.attempt
        ));
        self.dirty.insert(id.clone());
        let got = self
            .store
            .complete(
                &completion,
                failure.clone(),
                self.now,
                MAX_RETRIES,
                CORDON_MS,
            )
            .expect("complete with the current epoch");
        match outcome {
            Outcome::Done => {
                assert_eq!(got.state, JobState::Done);
                assert_eq!(got.finished_at, Some(self.now.wall_unix));
                assert_eq!(got.failure_class, None);
            }
            Outcome::Cancelled => {
                assert_eq!(got.state, JobState::Cancelled);
                assert_eq!(got.finished_at, Some(self.now.wall_unix));
            }
            Outcome::Failed(class) => {
                assert_eq!(got.failure_class, Some(class));
                if class.retryable(record.attempt, MAX_RETRIES) {
                    assert_eq!(got.state, JobState::Queued);
                    assert_eq!(got.attempt, record.attempt + 1);
                    assert_eq!(got.node, None);
                    assert_eq!(got.finished_at, None);
                    if class == FailureClass::Node {
                        let used = self.node_retries.entry(id.clone()).or_insert(0);
                        *used = used.saturating_add(1);
                        assert!(
                            *used <= MAX_RETRIES,
                            "node retries {used} exceed the cap {MAX_RETRIES}"
                        );
                    }
                    if class == FailureClass::Provider {
                        if let (Some(node), Some(provider)) =
                            (record.node.clone(), record.spec.provider.clone())
                        {
                            self.cordons.insert(
                                (node, provider),
                                self.now.mono_ms.saturating_add(CORDON_MS),
                            );
                        }
                    }
                } else {
                    assert_eq!(got.state, JobState::Failed);
                    assert_eq!(got.attempt, record.attempt);
                    assert_eq!(got.finished_at, Some(self.now.wall_unix));
                }
            }
        }
        self.lease_until.remove(&id);
        self.recovering_since.remove(&id);
    }

    fn op_cancel(&mut self) {
        if self.records.is_empty() {
            self.note("cancel: no job");
            return;
        }
        let ids: Vec<JobId> = self.records.keys().cloned().collect();
        let id = ids[self.rng.index(ids.len())].clone();
        let record = self.records.get(&id).expect("target").clone();
        let effect = record.state.cancel_effect();
        self.note(format!(
            "cancel {id} state={:?} effect={effect:?}",
            record.state
        ));
        self.dirty.insert(id.clone());
        let result = self.store.cancel_job(&id, self.now);
        match effect {
            lu_proto::CancelEffect::Now => {
                let got = result.expect("cancel now");
                assert_eq!(got.state, JobState::Cancelled);
                assert_eq!(got.finished_at, Some(self.now.wall_unix));
                self.lease_until.remove(&id);
                self.recovering_since.remove(&id);
            }
            lu_proto::CancelEffect::Request => {
                let got = result.expect("cancel request");
                assert_eq!(got.state, record.state);
                assert!(got.cancel_requested);
            }
            lu_proto::CancelEffect::Refuse => {
                assert_eq!(result.unwrap_err(), StoreError::Conflict { length: None });
            }
        }
    }

    fn op_drain(&mut self) {
        if self.rng.chance(1, 12) {
            let name = node("ghost");
            let draining = self.rng.chance(1, 2);
            self.note(format!(
                "drain unknown {} draining={draining}",
                name.as_str()
            ));
            assert_eq!(
                self.store.set_draining(&name, draining).unwrap_err(),
                StoreError::NotFound
            );
            return;
        }
        let name = node(NODE_NAMES[self.rng.index(NODE_NAMES.len())]);
        let draining = self.rng.chance(1, 2);
        self.note(format!("drain {} draining={draining}", name.as_str()));
        self.store
            .set_draining(&name, draining)
            .expect("drain a known node");
        if draining {
            self.draining.insert(name);
        } else {
            self.draining.remove(&name);
        }
    }

    fn op_expire(&mut self) {
        let expected: BTreeSet<JobId> = self
            .records
            .values()
            .filter(|record| {
                matches!(record.state, JobState::Leased | JobState::Running)
                    && self
                        .lease_until
                        .get(&record.id)
                        .is_some_and(|deadline| *deadline <= self.now.mono_ms)
            })
            .map(|record| record.id.clone())
            .collect();
        self.note(format!("expire_leases expected={expected:?}"));
        let got: BTreeSet<JobId> = self
            .store
            .expire_leases(self.now)
            .expect("expire_leases")
            .into_iter()
            .collect();
        assert_eq!(got, expected, "expire_leases moved the wrong jobs");
        for id in &got {
            self.lease_until.remove(id);
            self.recovering_since.insert(id.clone(), self.now.mono_ms);
            self.dirty.insert(id.clone());
        }
    }

    fn op_requeue(&mut self) {
        let due: BTreeSet<JobId> = self
            .records
            .values()
            .filter(|record| {
                record.state == JobState::Recovering
                    && self
                        .recovering_since
                        .get(&record.id)
                        .is_some_and(|since| since.saturating_add(GRACE_MS) <= self.now.mono_ms)
            })
            .map(|record| record.id.clone())
            .collect();
        self.note(format!("requeue_recovering due={due:?}"));
        let got: BTreeSet<JobId> = self
            .store
            .requeue_recovering(self.now, GRACE_MS, MAX_RETRIES)
            .expect("requeue_recovering")
            .into_iter()
            .collect();
        assert_eq!(got, due, "requeue_recovering moved the wrong jobs");
        for id in &got {
            let record = self.records.get(id).expect("moved job is known");
            let retry = FailureClass::Node.retryable(record.attempt, MAX_RETRIES);
            let after = self.store.get_job(id).expect("moved job is readable");
            if retry {
                assert_eq!(after.state, JobState::Queued);
                assert_eq!(after.attempt, record.attempt + 1);
                assert_eq!(after.node, None);
                assert_eq!(after.failure_class, None);
                assert_eq!(after.finished_at, None);
                assert!(!after.cancel_requested);
                let used = self.node_retries.entry(id.clone()).or_insert(0);
                *used = used.saturating_add(1);
                assert!(
                    *used <= MAX_RETRIES,
                    "node retries {used} exceed the cap {MAX_RETRIES}"
                );
            } else {
                assert_eq!(after.state, JobState::Failed);
                assert_eq!(after.failure_class, Some(FailureClass::Node));
                assert!(after.finished_at.is_some());
            }
            self.lease_until.remove(id);
            self.recovering_since.remove(id);
            self.dirty.insert(id.clone());
        }
    }

    fn op_recover_on_boot(&mut self) {
        let terminal: BTreeMap<JobId, JobRecord> = self
            .records
            .iter()
            .filter(|(_, record)| record.state.is_terminal())
            .map(|(id, record)| (id.clone(), record.clone()))
            .collect();
        let moving: Vec<JobId> = self
            .records
            .values()
            .filter(|record| matches!(record.state, JobState::Leased | JobState::Running))
            .map(|record| record.id.clone())
            .collect();
        let recovering: Vec<JobId> = self
            .records
            .values()
            .filter(|record| record.state == JobState::Recovering)
            .map(|record| record.id.clone())
            .collect();
        self.note(format!(
            "recover_on_boot moving={} already_recovering={}",
            moving.len(),
            recovering.len()
        ));
        let got = self
            .store
            .recover_on_boot(self.now)
            .expect("recover_on_boot");
        assert_eq!(got, moving.len() as u64);
        for id in moving {
            self.lease_until.remove(&id);
            self.recovering_since.insert(id.clone(), self.now.mono_ms);
            self.dirty.insert(id);
        }
        for id in recovering {
            self.recovering_since.insert(id.clone(), self.now.mono_ms);
            self.dirty.insert(id);
        }
        self.pending_terminal = Some(terminal);
    }

    /// Fetches only jobs the previous operation could have touched, plus any dependents whose
    /// terminal dependency changed. Re-reading every job here made the normal run far too slow.
    fn sync_dirty(&mut self) {
        while let Some(id) = self.dirty.pop_first() {
            let old = self.records.get(&id).cloned();
            let record = self.store.get_job(&id).expect("dirty job is readable");
            match &old {
                Some(old) => {
                    assert!(
                        old.state == record.state
                            || JobState::can_transition(old.state, record.state),
                        "illegal transition {:?} -> {:?} for {id}",
                        old.state,
                        record.state
                    );
                    assert!(
                        record.epoch >= old.epoch,
                        "epoch went backwards for {id}: {} -> {}",
                        old.epoch,
                        record.epoch
                    );
                    if record.epoch != old.epoch {
                        assert_eq!(record.epoch, old.epoch + 1, "epoch must bump by one");
                        assert_eq!(old.state, JobState::Queued);
                        assert_eq!(record.state, JobState::Leased);
                    }
                    assert!(
                        record.log_len >= old.log_len,
                        "log length went backwards for {id}"
                    );
                }
                None => {
                    assert!(
                        matches!(record.state, JobState::Blocked | JobState::Queued),
                        "a new job must start blocked or queued, got {:?}",
                        record.state
                    );
                    assert_eq!(record.epoch, 0);
                    assert_eq!(record.attempt, 1);
                    assert_eq!(record.node, None);
                    assert_eq!(record.finished_at, None);
                    assert_eq!(record.log_len, 0);
                }
            }
            if old.as_ref().map(|old| old.state) != Some(record.state) && record.state.is_terminal()
            {
                if let Some(dependents) = self.dependents.get(&id) {
                    self.dirty.extend(dependents.iter().cloned());
                }
            }
            self.records.insert(id, record);
        }
    }

    fn refresh_and_check(&mut self) {
        self.sync_dirty();

        if let Some(before) = self.pending_terminal.take() {
            for (id, old) in &before {
                let now = self
                    .records
                    .get(id)
                    .unwrap_or_else(|| panic!("recover_on_boot removed terminal job {id}"));
                assert_eq!(old, now, "recover_on_boot changed terminal job {id}");
            }
        }

        let current = &self.records;
        let mut holders: BTreeMap<NodeName, JobId> = BTreeMap::new();
        for record in current.values() {
            let mut not_done = BTreeSet::new();
            for dep in &record.spec.after {
                let dependency = current
                    .get(dep)
                    .unwrap_or_else(|| panic!("job {} depends on missing {dep}", record.id));
                if dependency.state != JobState::Done {
                    not_done.insert(dep.clone());
                }
            }
            assert!(
                !record.spec.after.contains(&record.id),
                "job {} depends on itself",
                record.id
            );
            assert_eq!(
                record.waiting_on.iter().cloned().collect::<BTreeSet<_>>(),
                not_done,
                "waiting_on for {}",
                record.id
            );
            match record.state {
                JobState::Blocked => {
                    assert!(
                        !not_done.is_empty(),
                        "blocked job {} has only done dependencies",
                        record.id
                    );
                    for dep in &not_done {
                        assert!(
                            !matches!(current[dep].state, JobState::Failed | JobState::Cancelled),
                            "blocked job {} waits on terminal dependency {dep}",
                            record.id
                        );
                    }
                    assert_eq!(record.finished_at, None);
                }
                JobState::Queued => {
                    assert!(
                        not_done.is_empty(),
                        "queued job {} has an unfinished dependency",
                        record.id
                    );
                    assert_eq!(record.finished_at, None);
                }
                JobState::Leased | JobState::Running | JobState::Recovering => {
                    assert!(
                        not_done.is_empty(),
                        "active job {} has an unfinished dependency",
                        record.id
                    );
                    assert!(
                        record.node.is_some(),
                        "active job {} has no node",
                        record.id
                    );
                    assert_eq!(record.finished_at, None);
                    if let Some(node) = &record.node {
                        if let Some(previous) = holders.insert(node.clone(), record.id.clone()) {
                            panic!(
                                "node {} has two active jobs: {} and {}",
                                node.as_str(),
                                previous,
                                record.id
                            );
                        }
                    }
                }
                JobState::Done | JobState::Failed | JobState::Cancelled => {
                    assert!(
                        record.finished_at.is_some(),
                        "terminal job {} has no finished_at",
                        record.id
                    );
                    assert!(
                        !self.lease_until.contains_key(&record.id),
                        "terminal job {} still has a lease",
                        record.id
                    );
                }
                JobState::NeedsInput => panic!("needs_input is not reachable"),
            }
            assert!(record.log_len <= MAX_LOG_BYTES);
            if let Some(provider) = &record.spec.provider {
                if let (Some(node), true) = (&record.node, active(record.state)) {
                    if let Some(until) = self.cordons.get(&(node.clone(), provider.clone())) {
                        assert!(
                            *until <= self.now.mono_ms,
                            "active job {} is on a node with an unexpired cordon for {provider}",
                            record.id
                        );
                    }
                    let unchanged =
                        self.node_generation.get(node) == self.assigned_hb.get(&record.id);
                    if unchanged {
                        assert!(
                            self.provider_ok(node, provider),
                            "active job {} uses a provider its node no longer reports ok",
                            record.id
                        );
                    }
                }
            }
            if let Some(used) = self.node_retries.get(&record.id) {
                assert!(
                    *used <= MAX_RETRIES,
                    "node retries {used} exceed the cap {MAX_RETRIES}"
                );
            }
            let expected = self
                .artifacts
                .get(&record.id)
                .unwrap_or_else(|| panic!("no artifact record for {}", record.id));
            let actual: BTreeSet<&'static str> =
                record.artifacts.iter().map(|a| a.file_name()).collect();
            assert_eq!(&actual, expected, "artifacts for {}", record.id);
        }

        for ((id, name), epoch) in &self.artifact_epochs {
            if let Some(record) = current.get(id) {
                assert!(
                    record.epoch >= *epoch,
                    "artifact {name} for {id} names an epoch that went backwards"
                );
            }
        }
        assert_acyclic(current);
    }
}

fn assert_acyclic(current: &BTreeMap<JobId, JobRecord>) {
    fn visit(id: &JobId, current: &BTreeMap<JobId, JobRecord>, colour: &mut BTreeMap<JobId, u8>) {
        match colour.get(id).copied() {
            Some(1) => panic!("dependency cycle through {id}"),
            Some(2) => return,
            _ => {}
        }
        colour.insert(id.clone(), 1);
        if let Some(record) = current.get(id) {
            for dep in &record.spec.after {
                visit(dep, current, colour);
            }
        }
        colour.insert(id.clone(), 2);
    }

    let mut colour = BTreeMap::new();
    for id in current.keys() {
        if !colour.contains_key(id) {
            visit(id, current, &mut colour);
        }
    }
}
